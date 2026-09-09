//! Validated budgets shared by isolated learning and evaluation requests.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::num::{NonZeroU32, NonZeroU64};

#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningExecutionConfig {
    /// Opt in to the provider's native JSON schema protocol. Keep false for
    /// endpoints whose selected model does not support structured output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<bool>,
    /// Total request deadline. Defaults to 120000 ms; maximum one hour.
    #[serde(
        default,
        deserialize_with = "timeout",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(range(max = 3_600_000))]
    pub timeout_ms: Option<NonZeroU64>,
    /// Serialized evidence input bytes. Defaults to 131072; maximum 1048576.
    #[serde(
        default,
        deserialize_with = "input_bytes",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(range(max = 1_048_576))]
    pub max_input_bytes: Option<NonZeroU32>,
    /// Output tokens per request. Defaults to 4096; maximum 65536.
    #[serde(
        default,
        deserialize_with = "output_tokens",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(range(max = 65_536))]
    pub max_output_tokens: Option<NonZeroU32>,
    /// Model steps in one isolated candidate evaluation. Defaults to 8; maximum 64.
    #[serde(
        default,
        deserialize_with = "steps",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(range(max = 64))]
    pub max_steps: Option<NonZeroU32>,
}

fn timeout<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<NonZeroU64>, D::Error> {
    let value = Option::<NonZeroU64>::deserialize(deserializer)?;
    if value.is_some_and(|value| value.get() > 3_600_000) {
        return Err(serde::de::Error::custom(
            "learning timeout_ms exceeds one hour",
        ));
    }
    Ok(value)
}

fn bounded<'de, D: Deserializer<'de>>(
    deserializer: D,
    maximum: u32,
) -> Result<Option<NonZeroU32>, D::Error> {
    let value = Option::<NonZeroU32>::deserialize(deserializer)?;
    if value.is_some_and(|value| value.get() > maximum) {
        return Err(serde::de::Error::custom(format!(
            "learning execution limit exceeds {maximum}"
        )));
    }
    Ok(value)
}

fn input_bytes<'de, D: Deserializer<'de>>(d: D) -> Result<Option<NonZeroU32>, D::Error> {
    bounded(d, 1_048_576)
}
fn output_tokens<'de, D: Deserializer<'de>>(d: D) -> Result<Option<NonZeroU32>, D::Error> {
    bounded(d, 65_536)
}
fn steps<'de, D: Deserializer<'de>>(d: D) -> Result<Option<NonZeroU32>, D::Error> {
    bounded(d, 64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn published_schema_and_deserialization_share_execution_ceilings() {
        let schema =
            serde_json::to_value(schemars::schema_for!(LearningExecutionConfig)).expect("schema");
        for (field, maximum) in [
            ("timeout_ms", 3_600_000),
            ("max_input_bytes", 1_048_576),
            ("max_output_tokens", 65_536),
            ("max_steps", 64),
        ] {
            assert_eq!(schema["properties"][field]["maximum"], json!(maximum));
            for invalid in [0, maximum + 1] {
                let value = json!({field:invalid});
                assert!(
                    serde_json::from_value::<LearningExecutionConfig>(value).is_err(),
                    "{field}"
                );
            }
            assert!(
                serde_json::from_value::<LearningExecutionConfig>(json!({field:maximum})).is_ok()
            );
        }
    }
}
