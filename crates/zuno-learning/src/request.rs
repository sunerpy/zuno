//! Pure request preparation shared by budget selection and provider admission.
use crate::{
    LearningModel, Result,
    model::{invalid, strict_schema},
};
use schemars::JsonSchema;
use serde_json::{Value, json};
use zuno_config::ResolvedLearningConfig;
use zuno_llm::{
    event::{Message, Role},
    registry::{ApiSurface, generation},
};

impl LearningModel {
    pub(crate) fn request_parameters(
        &self,
        limits: &ResolvedLearningConfig,
        schema: Option<&Value>,
    ) -> Result<serde_json::Map<String, Value>> {
        let mut parameters = self.parameters.clone();
        let mut output_limit = u64::from(limits.execution_max_output_tokens);
        if output_limit == 0 {
            return Err(invalid("learning execution output limit must be positive"));
        }
        for key in [
            "maxTokens",
            "max_tokens",
            "max_output_tokens",
            "max_completion_tokens",
        ] {
            if let Some(value) = parameters.remove(key) {
                let limit = value
                    .as_u64()
                    .ok_or_else(|| invalid("model output limit must be a non-negative integer"))?;
                // Like the foreground adapter, zero means no additional model cap.
                // The isolated request still keeps its positive execution ceiling.
                if limit > 0 {
                    output_limit = output_limit.min(limit);
                }
            }
        }
        // The provider's shared apply_parameters path lowers this single bounded
        // semantic cap after selecting its actual wire surface.
        parameters.insert(generation::MAX_TOKENS.to_owned(), json!(output_limit));
        if !self.sampling_params {
            for key in [
                "temperature",
                "topP",
                "top_p",
                "frequencyPenalty",
                "frequency_penalty",
                "presencePenalty",
                "presence_penalty",
            ] {
                parameters.remove(key);
            }
        }
        if limits.execution_structured_output
            && let Some(schema) = schema
        {
            match self.surface {
                ApiSurface::Chat => {
                    parameters.insert(
                        "response_format".to_owned(),
                        json!({"type":"json_schema","json_schema":{
                            "name":"learning_output","strict":true,"schema":schema}}),
                    );
                }
                ApiSurface::Responses => {
                    let text = parameters.entry("text").or_insert_with(|| json!({}));
                    let text = text
                        .as_object_mut()
                        .ok_or_else(|| invalid("Responses text options must be an object"))?;
                    text.insert("format".to_owned(), json!({
                        "type":"json_schema","name":"learning_output","strict":true,"schema":schema}));
                }
                ApiSurface::Messages => {
                    let output = parameters
                        .entry("output_config")
                        .or_insert_with(|| json!({}));
                    let output = output
                        .as_object_mut()
                        .ok_or_else(|| invalid("Messages output_config must be an object"))?;
                    output.insert(
                        "format".to_owned(),
                        json!({"type":"json_schema","schema":schema}),
                    );
                }
                ApiSurface::Default => {
                    return Err(invalid(
                        "structured output needs an explicit supported provider surface",
                    ));
                }
            }
        }
        Ok(parameters)
    }

    pub(crate) fn json_input_budget<T: JsonSchema>(
        &self,
        limits: &ResolvedLearningConfig,
        system: &str,
    ) -> Result<usize> {
        let schema = strict_schema::<T>();
        let parameters = self.request_parameters(limits, Some(&schema))?;
        payload_budget(
            system,
            &schema,
            &parameters,
            limits.execution_max_input_bytes,
        )
    }
}

pub(crate) fn json_messages(system: &str, schema: &Value, input: &str) -> Vec<Message> {
    vec![
        Message::new(
            Role::System,
            format!("{system}\nOutput JSON schema:\n{schema}"),
        ),
        Message::new(Role::User, input),
    ]
}

fn payload_budget(
    system: &str,
    schema: &Value,
    parameters: &serde_json::Map<String, Value>,
    maximum: u32,
) -> Result<usize> {
    let messages = json_messages(system, schema, "");
    let envelope = json!({"messages": messages, "parameters": parameters, "tools": []})
        .to_string()
        .len();
    // Input is serialized JSON placed inside a message string. Each byte is
    // already valid JSON text, so escaping quotes/backslashes adds at most one
    // byte per input byte. This bounds actual serialization, not token estimates.
    (maximum as usize)
        .checked_sub(envelope)
        .map(|remaining| remaining / 2)
        .filter(|remaining| *remaining > 0)
        .ok_or_else(|| invalid("learning prompt and schema exceed the serialized input budget"))
}

/// Enterprise profiles currently use no custom request parameters or structured
/// output option. The data owner can compute their exact envelope without a
/// provider, credential, journal, or local database.
pub fn learning_input_budget(
    phase: crate::distributed::LearningPhase,
    maximum_input_bytes: u32,
    maximum_output_tokens: u32,
) -> Result<usize> {
    if maximum_output_tokens == 0 {
        return Err(invalid("learning execution output limit must be positive"));
    }
    let parameters = serde_json::Map::from_iter([(
        generation::MAX_TOKENS.to_owned(),
        json!(maximum_output_tokens),
    )]);
    let (system, schema) = match phase {
        crate::distributed::LearningPhase::Extraction => (
            crate::model::extraction_prompt(),
            strict_schema::<crate::LearningExtraction>(),
        ),
        crate::distributed::LearningPhase::Maintenance => (
            crate::memory::memory_prompt(),
            strict_schema::<crate::MemoryConsolidation>(),
        ),
        crate::distributed::LearningPhase::SkillEvaluation => {
            return Err(invalid(
                "paired Skill evaluation validates its complete case and attempt envelopes",
            ));
        }
    };
    payload_budget(system, &schema, &parameters, maximum_input_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_payloads_fit_with_structured_schemas_parameters_and_escaped_text() {
        let schema = strict_schema::<crate::MemoryConsolidation>();
        for surface in [
            ApiSurface::Chat,
            ApiSurface::Responses,
            ApiSurface::Messages,
        ] {
            let model = LearningModel {
                provider_id: "test".to_owned(),
                model_id: "test".to_owned(),
                wire_id: "test".to_owned(),
                surface,
                parameters: serde_json::Map::from_iter([
                    ("reasoning".to_owned(), json!({"effort":"high"})),
                    (
                        "extra".to_owned(),
                        json!("quote \" and slash \\ ".repeat(30)),
                    ),
                ]),
                headers: Default::default(),
                sampling_params: false,
            };
            let limits = ResolvedLearningConfig {
                execution_max_input_bytes: 16384,
                execution_max_output_tokens: 512,
                execution_structured_output: true,
                ..Default::default()
            };
            let budget = model
                .json_input_budget::<crate::MemoryConsolidation>(&limits, "system")
                .unwrap();
            for symbol in ["\"", "\\", "\n", "\u{0000}", "中文", "🦀"] {
                let mut text = String::new();
                loop {
                    let next = format!("{text}{symbol}");
                    if json!({"text":next}).to_string().len() > budget {
                        break;
                    }
                    text = next;
                }
                let messages = json_messages("system", &schema, &json!({"text":text}).to_string());
                let parameters = model.request_parameters(&limits, Some(&schema)).unwrap();
                let actual =
                    json!({"messages":messages,"parameters":parameters,"tools":[]}).to_string();
                assert!(actual.len() <= 16384, "{surface:?}: {}", actual.len());
            }
        }
    }
}
