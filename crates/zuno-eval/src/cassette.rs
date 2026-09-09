//! An immutable recorded tool backend. There is deliberately no live fallback.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedCall {
    pub name: String,
    pub arguments: Value,
    pub output: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CassetteResult {
    pub matched: bool,
    pub output: String,
    pub is_error: bool,
}

pub struct CassetteDispatcher {
    calls: Vec<RecordedCall>,
    used: BTreeSet<usize>,
}

impl CassetteDispatcher {
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let mut calls = if let Some(calls) = value.get("calls") {
            serde_json::from_value::<Vec<RecordedCall>>(calls.clone())
                .map_err(|error| format!("invalid tool cassette: {error}"))?
        } else {
            Vec::new()
        };
        // Released observation-only suites remain readable and executable using
        // a read-only evidence tool. They never become fabricated shell results.
        if calls.is_empty() {
            for (index, evidence) in value
                .get("evidence")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                if let Some(text) = evidence.get("excerpt").and_then(Value::as_str) {
                    calls.push(RecordedCall {
                        name:"read_recorded_evidence".to_owned(),
                        arguments:json!({"source_id":evidence.get("sourceID").and_then(Value::as_str)
                            .map(str::to_owned).unwrap_or_else(||index.to_string())}),
                        output:text.to_owned(), is_error:false,
                    });
                }
            }
        }
        if calls.len() > 128
            || calls.iter().any(|call| {
                call.name.is_empty()
                    || call.name.len() > 128
                    || !call.arguments.is_object()
                    || call.output.len() > 65_536
                    || call.arguments.to_string().len() > 16_384
            })
        {
            return Err("tool cassette exceeds its bounds".to_owned());
        }
        Ok(Self {
            calls,
            used: BTreeSet::new(),
        })
    }

    pub fn calls(&self) -> &[RecordedCall] {
        &self.calls
    }

    pub fn dispatch(&mut self, name: &str, arguments: &Value) -> CassetteResult {
        let found = self.calls.iter().enumerate().find(|(index, call)| {
            !self.used.contains(index) && call.name == name && &call.arguments == arguments
        });
        if let Some((index, call)) = found {
            let result = CassetteResult {
                matched: true,
                output: call.output.clone(),
                is_error: call.is_error,
            };
            self.used.insert(index);
            return result;
        }
        CassetteResult {
            matched: false,
            is_error: true,
            output: "No matching recorded result exists. No command was executed.".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_arguments_and_consumption_are_required_without_live_fallback() {
        let mut dispatcher = CassetteDispatcher::from_value(&json!({"calls":[{
            "name":"shell","arguments":{"command":"cargo test"},"output":"passed","is_error":false
        }]}))
        .expect("cassette");
        assert!(
            !dispatcher
                .dispatch("shell", &json!({"command":"rm -rf /"}))
                .matched
        );
        assert!(!dispatcher.dispatch("network", &json!({})).matched);
        assert_eq!(
            dispatcher
                .dispatch("shell", &json!({"command":"cargo test"}))
                .output,
            "passed"
        );
        assert!(
            !dispatcher
                .dispatch("shell", &json!({"command":"cargo test"}))
                .matched
        );
    }
}
