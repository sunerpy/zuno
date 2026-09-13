//! Real paired model evaluation with recorded tools and durable case labels.
use crate::distributed::SkillEvaluationInput;
use crate::{LearningModelClient, LearningModelJournal, LearningModelRecord, Result};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use zuno_application::skill::{SkillCaseObservation, SkillCaseResult, SkillEvaluationReport};
use zuno_eval::{AttemptSnapshot, OfflineCaseEvaluator, OfflineCaseRequest};
use zuno_llm::event::{Message, RequestContentBlock, Role};

pub fn skill_attempt_trace(
    case: &zuno_application::skill::SkillEvaluationCase,
    outcomes: &[crate::LearningModelOutcome],
) -> Result<(Option<String>, Value, u64)> {
    let calls = case
        .calls
        .iter()
        .map(|call| zuno_eval::RecordedCall {
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            output: call.output.clone(),
            is_error: call.is_error,
        })
        .collect::<Vec<_>>();
    let mut cassette = zuno_eval::CassetteDispatcher::from_value(&json!({"calls":calls}))
        .map_err(|error| crate::model::invalid(&error))?;
    let mut trace = Vec::new();
    let mut unmatched = 0u64;
    let mut answer = None;
    for (step, outcome) in outcomes.iter().enumerate() {
        let crate::LearningModelOutcome::Completed {
            output, tool_calls, ..
        } = outcome
        else {
            return Err(crate::model::invalid(
                "failed model request cannot be an evaluation observation",
            ));
        };
        if answer.is_some() {
            return Err(crate::model::invalid(
                "evaluation continued after a final answer",
            ));
        }
        if tool_calls.is_empty() {
            answer = Some(output.clone());
            trace.push(json!({"step":step,"answer":output}));
            continue;
        }
        for call in tool_calls {
            let arguments: Value = serde_json::from_str(&call.arguments)
                .map_err(|_| crate::model::invalid("invalid recorded evaluation call"))?;
            let result = cassette.dispatch(&call.name, &arguments);
            if !result.matched {
                unmatched += 1;
            }
            trace.push(json!({"step":step,"tool":call.name,"arguments":arguments,"result":result}));
        }
    }
    Ok((answer, json!(trace), unmatched))
}

pub fn skill_request_identity<'a>(
    input: &'a SkillEvaluationInput,
    operation: &str,
) -> Option<(&'a zuno_application::skill::SkillEvaluationCase, bool, bool)> {
    for case in &input.cases {
        for (baseline, role) in [(true, "baseline"), (false, "candidate")] {
            for (grade, suffix) in [(false, "attempt"), (true, "grade")] {
                if operation == format!("skill.{}.{role}.learning.evaluation.{suffix}", case.id) {
                    return Some((case, baseline, grade));
                }
            }
        }
    }
    None
}
fn message_text(message: &Message) -> Option<&str> {
    match message.content.as_slice() {
        [RequestContentBlock::Text { text }] => Some(text),
        _ => None,
    }
}
pub fn validate_skill_model_request(
    input: &SkillEvaluationInput,
    record: &LearningModelRecord,
) -> bool {
    let Some((case, baseline, grade)) = skill_request_identity(input, &record.operation) else {
        return false;
    };
    let crate::LearningModelEvent::Request { request, tools, .. } = &record.event else {
        return false;
    };
    let Ok(messages) = serde_json::from_value::<Vec<Message>>(request["messages"].clone()) else {
        return false;
    };
    if messages.len() < 2
        || messages[0].role != Role::System
        || messages[1].role != Role::User
        || request["tools"] != json!(tools)
    {
        return false;
    }
    if grade {
        let Some(system) = message_text(&messages[0]) else {
            return false;
        };
        let Some(user) = message_text(&messages[1]) else {
            return false;
        };
        let Ok(value) = serde_json::from_str::<Value>(user) else {
            return false;
        };
        return tools.is_empty()
            && system.starts_with(crate::evaluator::GRADE_PROMPT)
            && value["scenario"] == case.prompt
            && value["expected"] == case.expected;
    }
    let content = if baseline {
        &input.baseline_content
    } else {
        &input.proposed_content
    };
    let calls = case
        .calls
        .iter()
        .map(|call| zuno_eval::RecordedCall {
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            output: call.output.clone(),
            is_error: call.is_error,
        })
        .collect::<Vec<_>>();
    let expected = crate::evaluator::recorded_tool_schemas(&calls)
        .into_iter()
        .map(|tool| {
            json!({
                "name":tool.name,"description":tool.description,"parameters":tool.parameters,
            })
        })
        .collect::<Vec<_>>();
    message_text(&messages[0]) == Some(crate::evaluator::attempt_prompt(content).as_str())
        && message_text(&messages[1]) == Some(case.prompt.as_str())
        && tools == &expected
}

struct CaseJournal {
    inner: Arc<dyn LearningModelJournal>,
    prefix: String,
}
#[async_trait]
impl LearningModelJournal for CaseJournal {
    async fn record(&self, mut record: LearningModelRecord) -> Result<()> {
        record.operation = format!("{}.{}", self.prefix, record.operation);
        self.inner.record(record).await
    }
}
impl LearningModelClient {
    pub async fn evaluate_skill(
        &self,
        input: &SkillEvaluationInput,
    ) -> Result<SkillEvaluationReport> {
        input
            .validate()
            .map_err(|error| crate::model::invalid(&error.to_string()))?;
        if self.limits.execution_max_steps != input.maximum_steps {
            return Err(crate::model::invalid(
                "Skill evaluation step limit differs from its installed profile",
            ));
        }
        let mut results = Vec::new();
        for case in &input.cases {
            let calls = case
                .calls
                .iter()
                .map(|call| zuno_eval::RecordedCall {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    output: call.output.clone(),
                    is_error: call.is_error,
                })
                .collect::<Vec<_>>();
            let cassette = serde_json::json!({"mode":"recorded-only","calls":calls});
            let attempt = AttemptSnapshot {
                model: format!("{}/{}", self.model.provider_id, self.model.model_id),
                toolset_digest: zuno_orchestration::sha256_json(&cassette),
                max_output_tokens: self.limits.execution_max_output_tokens,
                max_steps: input.maximum_steps,
                temperature_millis: 0,
                seed: 0,
            };
            let mut observations = Vec::new();
            for (role, content) in [
                ("baseline", &input.baseline_content),
                ("candidate", &input.proposed_content),
            ] {
                let mut client = self.clone();
                client.journal = Arc::new(CaseJournal {
                    inner: self.journal.clone(),
                    prefix: format!("skill.{}.{}", case.id, role),
                });
                let evaluator = crate::ProviderSkillEvaluator {
                    client,
                    session_id: input.session_id.clone(),
                };
                let value = evaluator
                    .evaluate(OfflineCaseRequest {
                        case_id: case.id.to_string(),
                        skill_content: content.clone(),
                        prompt: case.prompt.clone(),
                        expected: case.expected.clone(),
                        tool_cassette: cassette.clone(),
                        attempt: attempt.clone(),
                    })
                    .await
                    .map_err(|source| {
                        crate::LearningServiceError::Evaluation(
                            zuno_eval::EvaluationError::Evaluator {
                                case_id: case.id.to_string(),
                                source,
                            },
                        )
                    })?;
                observations.push(SkillCaseObservation {
                    score: u8::try_from(value.score)
                        .map_err(|_| crate::model::invalid("invalid evaluation score"))?,
                    passed: value.passed,
                    critical_failure: value.critical_failure,
                    details: value.details,
                });
            }
            let candidate = observations.pop().expect("candidate");
            let baseline = observations.pop().expect("baseline");
            results.push(SkillCaseResult {
                case_id: case.id.clone(),
                baseline,
                candidate,
            });
        }
        SkillEvaluationReport::from_cases(&input.cases, results)
            .map_err(|error| crate::model::invalid(&error.to_string()))
    }
}
