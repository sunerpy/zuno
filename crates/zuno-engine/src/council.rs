//! Provider-independent Council answer validation. Runtime hosts own execution,
//! authorization, retries, evidence anchoring and synthesis.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
