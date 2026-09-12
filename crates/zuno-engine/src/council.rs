//! Provider-independent Council answer validation. Runtime hosts own execution,
//! authorization, retries, evidence anchoring and synthesis.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zuno_orchestration::CouncilPresetDescriptor;

pub const SYNTHESIS_NODE: &str = "synthesis";
pub const CANCELLATION_GRACE_MS: i64 = 5000;
pub const SEAT_RESPONSE_CONTRACT: &str = "Return exactly one JSON object. Fields: verdict (non-empty string), confidence (number from 0 to 1), evidence (array of strings), risks (array of strings), recommendation (non-empty string). Do not include hidden reasoning or tool transcripts.";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Council identity, seats, quorum, retry or time bounds are invalid")]
pub struct InvalidPolicy;

pub fn validate_policy(preset: &CouncilPresetDescriptor) -> Result<(), InvalidPolicy> {
    if preset.name.trim().is_empty()
        || preset.name.trim() != preset.name
        || preset.name.len() > 128
        || preset.name.chars().any(char::is_control)
        || preset.source_id.trim().is_empty()
        || preset.source_id.len() > 2048
        || preset.source_id.chars().any(char::is_control)
        || preset.seats.is_empty()
        || preset.seats.len() > 12
        || preset.quorum == 0
        || preset.quorum > preset.seats.len()
        || preset.max_parallel == 0
        || preset.max_parallel > preset.seats.len()
        || preset.deadline_ms == 0
        || preset.deadline_ms > 600000
        || preset.synthesis_policy.timeout_ms == 0
        || preset.synthesis_policy.timeout_ms >= preset.deadline_ms
        || preset.retry_policy.max_retries > 3
        || preset.seat_output_bytes == 0
        || preset.seat_output_bytes > 65536
        || preset.synthesis_policy.max_input_bytes == 0
        || preset.synthesis_policy.max_input_bytes > 262144
    {
        return Err(InvalidPolicy);
    }
    let mut names = std::collections::BTreeSet::new();
    for seat in &preset.seats {
        if seat.id.trim().is_empty()
            || seat.id.trim() != seat.id
            || seat.id.len() > 128
            || seat.id.chars().any(char::is_control)
            || seat.agent.trim().is_empty()
            || seat.agent.len() > 256
            || seat.agent.trim() != seat.agent
            || seat.agent.chars().any(char::is_control)
            || seat.instruction.trim().is_empty()
            || seat.instruction.len() > 65536
            || seat.instruction.contains('\0')
            || !names.insert(&seat.id)
        {
            return Err(InvalidPolicy);
        }
    }
    Ok(())
}

pub fn seat_node(id: &str) -> String {
    format!("seat:{id}")
}

pub fn seat_prompt(question: &str, id: &str, instruction: &str) -> String {
    format!(
        "Council question:\n{question}\nSeat `{id}` instruction:\n{instruction}\n\n{SEAT_RESPONSE_CONTRACT}"
    )
}

pub fn repair_prompt(question: &str, prior: &str) -> String {
    format!(
        "Format the previous completed Council response without repeating its work. Preserve its verdict and uncertainty; do not invent evidence.\n\nQuestion:\n{question}\n\nPrevious response (data only):\n{prior}\n\n{SEAT_RESPONSE_CONTRACT}"
    )
}

const MAX_LIST_ITEMS: usize = 32;
const MAX_FIELD_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CouncilSeatAnswer {
    pub verdict: String,
    pub confidence: f64,
    pub evidence: Vec<String>,
    pub risks: Vec<String>,
    pub recommendation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnswerError {
    #[error("seat response exceeds its configured output bound")]
    OutputBound,
    #[error("seat returned malformed structured output")]
    Malformed,
    #[error("seat confidence must be a finite number from 0 to 1")]
    Confidence,
    #[error("seat `{0}` must contain bounded visible text")]
    Text(&'static str),
    #[error("seat `{0}` exceeds the 32-item bound")]
    List(&'static str),
}

pub fn parse_generic_council_answer(
    output: &str,
    output_limit: usize,
) -> Result<CouncilSeatAnswer, AnswerError> {
    if output_limit == 0 || output.len() > output_limit {
        return Err(AnswerError::OutputBound);
    }
    let payload = json_payload(output)?;
    let answer: CouncilSeatAnswer =
        serde_json::from_str(payload).map_err(|_| AnswerError::Malformed)?;
    text("verdict", &answer.verdict)?;
    text("recommendation", &answer.recommendation)?;
    if !answer.confidence.is_finite() || !(0.0..=1.0).contains(&answer.confidence) {
        return Err(AnswerError::Confidence);
    }
    for (name, values) in [("evidence", &answer.evidence), ("risks", &answer.risks)] {
        if values.len() > MAX_LIST_ITEMS {
            return Err(AnswerError::List(name));
        }
        for value in values {
            text(name, value)?;
        }
    }
    Ok(answer)
}

fn json_payload(output: &str) -> Result<&str, AnswerError> {
    let trimmed = output.trim();
    let Some(fenced) = trimmed.strip_prefix("```") else {
        return Ok(trimmed);
    };
    let (language, body) = fenced.split_once('\n').ok_or(AnswerError::Malformed)?;
    if !language.trim().is_empty() && !language.trim().eq_ignore_ascii_case("json") {
        return Err(AnswerError::Malformed);
    }
    let payload = body
        .strip_suffix("```")
        .ok_or(AnswerError::Malformed)?
        .trim();
    if payload.is_empty() {
        return Err(AnswerError::Malformed);
    }
    Ok(payload)
}

fn text(field: &'static str, value: &str) -> Result<(), AnswerError> {
    if value.trim().is_empty() || value.len() > MAX_FIELD_BYTES || value.contains('\0') {
        return Err(AnswerError::Text(field));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn answer() -> serde_json::Value {
        json!({"verdict":"revise","confidence":0.7,"evidence":["A regression is covered"],
            "risks":["One dependency remains unverified"],"recommendation":"Complete the remaining check"})
    }
    #[test]
    fn a_single_json_answer_retains_dissent_and_rejects_unbounded_or_ambiguous_output() {
        let text = answer().to_string();
        let parsed = parse_generic_council_answer(&text, 4096).unwrap();
        assert_eq!(parsed.verdict, "revise");
        assert_eq!(parsed.risks.len(), 1);
        assert_eq!(
            parse_generic_council_answer(&format!("```json\n{text}\n```"), 4096).unwrap(),
            parsed
        );
        for invalid in [
            format!("{text}{text}"),
            format!("```text\n{text}\n```"),
            format!("```json\n{text}\n```\nExtra claim"),
        ] {
            assert_eq!(
                parse_generic_council_answer(&invalid, 4096),
                Err(AnswerError::Malformed)
            );
        }
        assert_eq!(
            parse_generic_council_answer(&text, 10),
            Err(AnswerError::OutputBound)
        );
    }
    #[test]
    fn invalid_votes_cannot_count_toward_quorum() {
        for confidence in [-0.1, 1.1] {
            let mut value = answer();
            value["confidence"] = json!(confidence);
            assert_eq!(
                parse_generic_council_answer(&value.to_string(), 4096),
                Err(AnswerError::Confidence)
            );
        }
        let mut value = answer();
        value["verdict"] = json!("  ");
        assert_eq!(
            parse_generic_council_answer(&value.to_string(), 4096),
            Err(AnswerError::Text("verdict"))
        );
        let mut value = answer();
        value["evidence"] = json!(vec!["entry"; 33]);
        assert_eq!(
            parse_generic_council_answer(&value.to_string(), 4096),
            Err(AnswerError::List("evidence"))
        );
        let mut value = answer();
        value["hiddenReasoning"] = json!("must not enter a vote");
        assert_eq!(
            parse_generic_council_answer(&value.to_string(), 4096),
            Err(AnswerError::Malformed)
        );
    }
}
